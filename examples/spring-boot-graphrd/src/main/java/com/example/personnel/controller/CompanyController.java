package com.example.personnel.controller;

import java.util.List;

import org.springframework.http.HttpStatus;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

import com.example.personnel.model.dto.CompanyIn;
import com.example.personnel.model.entity.Company;
import com.example.personnel.service.CompanyService;

/** Companies: what the employees join to — a pure shell over the service. */
@RestController
@RequestMapping("/companies")
class CompanyController {

    private final CompanyService companies;

    CompanyController(CompanyService companies) {
        this.companies = companies;
    }

    @GetMapping
    List<Company> all() {
        return companies.all();
    }

    @PostMapping
    ResponseEntity<Company> create(@RequestBody CompanyIn in) {
        return ResponseEntity.status(HttpStatus.CREATED).body(companies.create(in));
    }
}
