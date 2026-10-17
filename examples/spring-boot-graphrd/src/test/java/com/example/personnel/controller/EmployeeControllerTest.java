package com.example.personnel.controller;

import static org.mockito.ArgumentMatchers.any;
import static org.mockito.Mockito.when;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.delete;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.post;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.content;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

import java.util.List;
import java.util.NoSuchElementException;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.extension.ExtendWith;
import org.mockito.InjectMocks;
import org.mockito.Mock;
import org.mockito.junit.jupiter.MockitoExtension;
import org.springframework.data.domain.PageImpl;
import org.springframework.data.domain.Pageable;
import org.springframework.http.MediaType;
import org.springframework.test.web.servlet.MockMvc;
import org.springframework.test.web.servlet.setup.MockMvcBuilders;

import com.example.personnel.model.dto.EmployeeIn;
import com.example.personnel.model.dto.EmployeeOut;
import com.example.personnel.model.dto.RdfRepresentation;
import com.example.personnel.model.filter.EmployeeSearch;
import com.example.personnel.service.EmployeeService;

/** The employee endpoints as pure HTTP: binding, status codes, error shape — the service is a mock. */
@ExtendWith(MockitoExtension.class)
class EmployeeControllerTest {

    private static final String ID = "http://example.org/employee/alice";
    private static final String QID = "?id=" + java.net.URLEncoder.encode(ID, java.nio.charset.StandardCharsets.UTF_8);

    @Mock
    private EmployeeService employees;

    @InjectMocks
    private EmployeeController controller;

    private MockMvc mvc;

    @org.junit.jupiter.api.BeforeEach
    void setUp() {
        var jackson = new org.springframework.http.converter.json.MappingJackson2HttpMessageConverter(
                new com.fasterxml.jackson.databind.ObjectMapper()
                        .registerModule(new org.springframework.data.web.config.SpringDataJacksonConfiguration().pageModule()));
        var strings = new org.springframework.http.converter.StringHttpMessageConverter();
        mvc = MockMvcBuilders.standaloneSetup(controller)
                .setControllerAdvice(new com.example.personnel.exception.ApiExceptionHandler())
                .setCustomArgumentResolvers(new org.springframework.data.web.PageableHandlerMethodArgumentResolver())
                .setMessageConverters(jackson, strings)
                .build();
    }

    private static EmployeeOut alice() {
        return new EmployeeOut(ID, "Alice", "alice@example.org", 34, 82000, "2021-03-15", "Acme");
    }

    @Test
    void listingBindsSearchAndPageable() throws Exception {
        when(employees.search(any(EmployeeSearch.class), any(Pageable.class)))
                .thenReturn(new PageImpl<>(List.of(alice())));
        mvc.perform(get("/employees").param("minAge", "30")
                        .param("sort", "salary,desc").param("size", "20"))
                .andExpect(status().isOk())
                .andExpect(content().contentTypeCompatibleWith(MediaType.APPLICATION_JSON))
                .andExpect(jsonPath("$.content[0].name").value("Alice"))
                .andExpect(jsonPath("$.content[0].companyName").value("Acme"));
    }

    @Test
    void oneAsJson() throws Exception {
        when(employees.get(any())).thenReturn(alice());
        mvc.perform(get("/employees/one" + QID).accept(MediaType.APPLICATION_JSON))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$.id").value(ID))
                .andExpect(jsonPath("$.salary").value(82000));
    }

    @Test
    void oneAsTurtleWhenAsked() throws Exception {
        when(employees.oneAsRdf(any(), org.mockito.ArgumentMatchers.contains("turtle")))
                .thenReturn(new RdfRepresentation("text/turtle", "<x> a <Turtle> ."));
        mvc.perform(get("/employees/one" + QID).accept("text/turtle"))
                .andExpect(status().isOk())
                .andExpect(content().contentType("text/turtle"))
                .andExpect(content().string("<x> a <Turtle> ."));
    }

    @Test
    void oneAsJsonLdWhenAsked() throws Exception {
        when(employees.oneAsRdf(any(), org.mockito.ArgumentMatchers.contains("ld+json")))
                .thenReturn(new RdfRepresentation("application/ld+json", "{\"@id\":\"x\"}"));
        mvc.perform(get("/employees/one" + QID).accept("application/ld+json"))
                .andExpect(status().isOk())
                .andExpect(content().contentType("application/ld+json"))
                .andExpect(content().string(org.hamcrest.Matchers.containsString("@id")));
    }

    @Test
    void theWholeListingAsRdf() throws Exception {
        when(employees.listAsRdf("application/n-triples"))
                .thenReturn(new RdfRepresentation("application/n-triples", "<a> <b> <c> .\n"));
        mvc.perform(get("/employees/rdf").accept("application/n-triples"))
                .andExpect(status().isOk())
                .andExpect(content().contentType("application/n-triples"))
                .andExpect(content().string(org.hamcrest.Matchers.containsString("<a> <b> <c> .")));
    }

    @Test
    void createAnswers201WithTheBody() throws Exception {
        when(employees.create(any(EmployeeIn.class))).thenReturn(alice());
        mvc.perform(post("/employees").contentType(MediaType.APPLICATION_JSON)
                        .content("{\"name\":\"Alice\",\"email\":\"alice@example.org\",\"age\":34,"
                                + "\"salary\":82000,\"company\":\"http://example.org/company/acme\"}"))
                .andExpect(status().isCreated())
                .andExpect(jsonPath("$.name").value("Alice"));
    }

    @Test
    void validationAnswersTheOneErrorShape() throws Exception {
        when(employees.create(any(EmployeeIn.class)))
                .thenThrow(new IllegalArgumentException("email already in use: a@b.c"));
        mvc.perform(post("/employees").contentType(MediaType.APPLICATION_JSON)
                        .content("{\"name\":\"X\",\"email\":\"a@b.c\"}"))
                .andExpect(status().isBadRequest())
                .andExpect(jsonPath("$.status").value("BAD_REQUEST"))
                .andExpect(jsonPath("$.message").value("email already in use: a@b.c"));
    }

    @Test
    void missingEmployeeAnswers404ThroughTheHandler() throws Exception {
        when(employees.get(any())).thenThrow(new NoSuchElementException("no such employee: " + ID));
        mvc.perform(get("/employees/one" + QID).accept(MediaType.APPLICATION_JSON))
                .andExpect(status().isNotFound())
                .andExpect(jsonPath("$.status").value("NOT_FOUND"));
    }

    @Test
    void deleteIsANoContentShell() throws Exception {
        mvc.perform(delete("/employees/one" + QID)).andExpect(status().isOk());
    }
}
