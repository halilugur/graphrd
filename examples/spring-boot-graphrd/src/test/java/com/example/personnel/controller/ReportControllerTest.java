package com.example.personnel.controller;

import static org.mockito.Mockito.when;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

import java.util.List;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.extension.ExtendWith;
import org.mockito.InjectMocks;
import org.mockito.Mock;
import org.mockito.junit.jupiter.MockitoExtension;
import org.springframework.test.web.servlet.MockMvc;
import org.springframework.test.web.servlet.setup.MockMvcBuilders;

import com.example.personnel.model.dto.PayrollRow;
import com.example.personnel.model.dto.Totals;
import com.example.personnel.service.PayrollService;

/** The report endpoints: pure passthroughs over a mocked service. */
@ExtendWith(MockitoExtension.class)
class ReportControllerTest {

    @Mock
    private PayrollService payroll;

    @InjectMocks
    private ReportController controller;

    private MockMvc mvc;

    @org.junit.jupiter.api.BeforeEach
    void setUp() {
        var standalones = MockMvcBuilders.standaloneSetup(controller);
        standalones.setControllerAdvice(new com.example.personnel.exception.ApiExceptionHandler());
        mvc = standalones.build();
    }

    @Test
    void payrollRowsAsTheyCome() throws Exception {
        when(payroll.payroll()).thenReturn(List.of(
                new PayrollRow("Acme", 2, 143000, 71500.0, 61000, 82000)));
        mvc.perform(get("/reports/payroll"))
                .andExpect(status().isOk())
                .andExpect(jsonPath("$[0].company").value("Acme"))
                .andExpect(jsonPath("$[0].heads").value(2))
                .andExpect(jsonPath("$[0].average").value(71500.0));
    }

    @Test
    void totals() throws Exception {
        when(payroll.totals()).thenReturn(new Totals(5, 3, 367000, 73400.0, 1));
        mvc.perform(get("/reports/totals"))
                .andExpect(jsonPath("$.employees").value(5))
                .andExpect(jsonPath("$.payroll").value(367000));
    }

    @Test
    void raiseCostWithAndWithoutSince() throws Exception {
        when(payroll.raiseCost()).thenReturn(4000L);
        when(payroll.raiseCostSince("2026-01-01")).thenReturn(1000L);
        mvc.perform(get("/reports/raise-cost")).andExpect(jsonPath("$").value(4000));
        mvc.perform(get("/reports/raise-cost").param("since", "2026-01-01"))
                .andExpect(jsonPath("$").value(1000));
    }

    @Test
    void oneCompanyRowByName() throws Exception {
        when(payroll.companyPayroll("Acme")).thenReturn(new PayrollRow("Acme", 2, 143000, 71500.0, 61000, 82000));
        mvc.perform(get("/reports/payroll/company").param("name", "Acme"))
                .andExpect(jsonPath("$.company").value("Acme"));
    }
}
