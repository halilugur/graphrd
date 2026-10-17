package com.example.personnel.controller;

import static org.mockito.ArgumentMatchers.any;
import static org.mockito.Mockito.when;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.post;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

import java.util.List;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.extension.ExtendWith;
import org.mockito.InjectMocks;
import org.mockito.Mock;
import org.mockito.junit.jupiter.MockitoExtension;
import org.springframework.http.MediaType;
import org.springframework.test.web.servlet.MockMvc;
import org.springframework.test.web.servlet.setup.MockMvcBuilders;

import com.example.personnel.model.dto.CompanyIn;
import com.example.personnel.model.entity.Company;
import com.example.personnel.service.CompanyService;

/** The company endpoints: binding and status codes over a mocked service. */
@ExtendWith(MockitoExtension.class)
class CompanyControllerTest {

    @Mock
    private CompanyService companies;

    @InjectMocks
    private CompanyController controller;

    private MockMvc mvc;

    @org.junit.jupiter.api.BeforeEach
    void setUp() {
        var standalones = MockMvcBuilders.standaloneSetup(controller);
        standalones.setControllerAdvice(new com.example.personnel.exception.ApiExceptionHandler());
        mvc = standalones.build();
    }

    @Test
    void listsAll() throws Exception {
        when(companies.all()).thenReturn(List.of(
                new Company("http://example.org/company/acme", "Acme", "manufacturing"),
                new Company("http://example.org/company/init", "Initech", "software")));
        mvc.perform(get("/companies"))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$[0].name").value("Acme"))
                .andExpect(jsonPath("$[1].industry").value("software"));
    }

    @Test
    void createAnswers201WithTheEntity() throws Exception {
        when(companies.create(any(CompanyIn.class)))
                .thenReturn(new Company("http://example.org/company/acme", "Acme", "manufacturing"));
        mvc.perform(post("/companies").contentType(MediaType.APPLICATION_JSON)
                        .content("{\"name\":\"Acme\",\"industry\":\"manufacturing\"}"))
                .andExpect(status().isCreated())
                .andExpect(jsonPath("$.id").value("http://example.org/company/acme"));
    }

    @Test
    void blankNameIsTheErrorShape() throws Exception {
        when(companies.create(any(CompanyIn.class))).thenThrow(new IllegalArgumentException("the name is required"));
        mvc.perform(post("/companies").contentType(MediaType.APPLICATION_JSON).content("{\"name\":\" \"}"))
                .andExpect(status().isBadRequest())
                .andExpect(jsonPath("$.message").value("the name is required"));
    }
}
